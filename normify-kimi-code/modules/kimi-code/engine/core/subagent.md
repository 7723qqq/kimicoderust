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
      
revision: 932e017600564470d7e43d7b25bb49896c06cca1
updated_at: "2026-11-01T08:11:47.974Z"
fingerprint: 2e34af99a2353535ac19f4c73abc9cf31b02ab52e009884e77e65c5f762a3091
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
