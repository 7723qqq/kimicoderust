---
uid: e1e00005
id: kimi-code.engine.tools.agent
parent: kimi-code.engine.tools
name: {zh: "Agent 工具族", en: "Agent tools"}
description:
  zh: >
      注册工具名：Agent、AgentSwarm、define_subagent、invoke_subagent、manage_subagents。
      
  en: >
      Registered tool names: Agent, AgentSwarm, define_subagent, invoke_subagent, manage_subagents.
      
revision: 2c47ec0cfa7f2a2418c6b44ec10ec14d29f5f7cf
updated_at: "2026-11-15T18:51:51.088Z"
fingerprint: be896956cce29f9f425f1acd910fc240285a5911789c6fcc18c125cba6c9a259
source:
  - path: "packages/kimi-agent/src/tools/agent_tool.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/defs.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/events.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/execute.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/format.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/resume.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/tests/mod.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/tests/common.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/tests/display.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/tests/events.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/tests/fork_background.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/tests/limits.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/tests/pool.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/tests/resume.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/tests/spawn_contract.rs"
  - path: "packages/kimi-agent/src/tools/agent_tool/tests/stopped_output.rs"
  - path: "packages/kimi-agent/src/tools/subagent_tools.rs"
  - path: "packages/kimi-agent/src/tools/select_tools.rs"
apis:
  - protocol: rpc
    path: "Agent"
    description:
      zh: >
          派生或寻址子代理
          
      en: >
          Spawn or address a subagent
          
  - protocol: rpc
    path: "AgentSwarm"
    description:
      zh: >
          并行 agent 集群
          
      en: >
          Parallel agent swarm
          
  - protocol: rpc
    path: "define_subagent"
    description:
      zh: >
          注册子代理画像
          
      en: >
          Register subagent profiles
          
  - protocol: rpc
    path: "invoke_subagent"
    description:
      zh: >
          调用已注册子代理
          
      en: >
          Invoke a registered subagent
          
  - protocol: rpc
    path: "manage_subagents"
    description:
      zh: >
          管理已注册子代理
          
      en: >
          Manage registered subagents
          
---
