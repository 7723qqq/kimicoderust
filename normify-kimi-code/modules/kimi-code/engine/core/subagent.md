---
uid: e1e00003
id: kimi-code.engine.core.subagent
parent: kimi-code.engine.core
name: {zh: "子代理", en: "Subagents"}
description:
  zh: >
      子代理编排：manager、fork、persistent/secondary 代理与 btw 侧信道。
      
  en: >
      Subagent orchestration: manager, fork, persistent and secondary agents, the btw side-channel.
      
revision: 6db9312678bef88d59836b57ae3e9fb1173e33f9
updated_at: "2026-11-06T15:36:17.070Z"
fingerprint: 1a3627d9aaf893a354fbdcf02f63fe3f012382ad275340a1270dfaf0a64e6ff0
source:
  - path: "packages/kimi-agent/src/subagent/manager.rs"
  - path: "packages/kimi-agent/src/subagent/fork.rs"
  - path: "packages/kimi-agent/src/subagent/btw.rs"
apis: []
deps:
  - kind: call
    to: kimi-code.engine.core.turn-loop
    label: {zh: "驱动子回合", en: "runs sub turns"}
---
