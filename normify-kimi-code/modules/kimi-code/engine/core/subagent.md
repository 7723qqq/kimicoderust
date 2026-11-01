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
      
revision: b46cc711fbf0f7be46d03a1ff637613ab2b7e8a3
updated_at: "2026-11-01T07:59:04.306Z"
fingerprint: ffbba0f38ba9b0718b4d32b2a9ffff5c8787bcae165ce0a3fe5460fb5c39a043
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
