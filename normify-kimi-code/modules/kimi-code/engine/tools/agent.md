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
      
revision: 3b18eb137f8d3057962e2f89ee9e34901ced8df4
updated_at: "2026-11-07T02:35:09.748Z"
fingerprint: 6ca3330f7c8bebbff3146bc661e92933c6a44a87dbbed6d7bf6d80307df90a56
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
