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
      
revision: 3b18eb137f8d3057962e2f89ee9e34901ced8df4
updated_at: "2026-11-07T02:14:00.807Z"
fingerprint: 0494076dd6e5153ff994e9eef3c0abaf2e29832305667568a518656a3e3a7da6
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
