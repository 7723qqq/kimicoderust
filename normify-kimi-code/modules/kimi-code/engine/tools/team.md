---
uid: e1e00006
id: kimi-code.engine.tools.team
parent: kimi-code.engine.tools
name: {zh: "团队与规划", en: "Team & planning"}
description:
  zh: >
      注册工具名：Team、EnterPlanMode、ExitPlanMode、TodoList。
      
  en: >
      Registered tool names: Team, EnterPlanMode, ExitPlanMode, TodoList.
      
revision: 3b18eb137f8d3057962e2f89ee9e34901ced8df4
updated_at: "2026-11-06T22:57:55.041Z"
fingerprint: 6437e000546d5d9bbbefc15d295e7e7cb21aa44b2815aaac623ae65e2f0c5033
source:
  - path: "packages/kimi-agent/src/tools/team_tool.rs"
  - path: "packages/kimi-agent/src/tools/swarm_tool.rs"
  - path: "packages/kimi-agent/src/tools/plan_mode.rs"
  - path: "packages/kimi-agent/src/tools/todo_list.rs"
apis:
  - protocol: rpc
    path: "Team"
    description:
      zh: >
          多 agent 辩论与共识
          
      en: >
          Multi-agent debate and consensus
          
  - protocol: rpc
    path: "EnterPlanMode"
    description:
      zh: >
          进入计划模式
          
      en: >
          Enter planning mode
          
  - protocol: rpc
    path: "ExitPlanMode"
    description:
      zh: >
          携带计划退出计划模式
          
      en: >
          Exit planning mode with the plan
          
  - protocol: rpc
    path: "TodoList"
    description:
      zh: >
          持久化待办清单
          
      en: >
          Persisted todo list
          
---
