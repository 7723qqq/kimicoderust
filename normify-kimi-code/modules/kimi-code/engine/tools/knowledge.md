---
uid: e1e00007
id: kimi-code.engine.tools.knowledge
parent: kimi-code.engine.tools
name: {zh: "知识与记忆", en: "Knowledge & memory"}
description:
  zh: >
      注册工具名：Skill、Knowledge、Memory（统一 action：search/read/write/list/delete/log/revert）、session_query（wire 事件检索）、lsp（实验性，默认关闭）。
      
  en: >
      Registered tool names: Skill, Knowledge, Memory (unified action: search/read/write/list/delete/log/revert), session_query (wire-event search), lsp (experimental, off by default).
      
revision: 8c50a0dfdddde7e560ee7637ad3b533551547ccc
updated_at: "2026-11-09T19:52:51.322Z"
fingerprint: 738b3e9e791a5eea29001b0b1accda425dee662905af8f2bee30ea2509060eca
source:
  - path: "packages/kimi-agent/src/tools/skill.rs"
  - path: "packages/kimi-agent/src/tools/memory_tool.rs"
  - path: "packages/kimi-agent/src/tools/memory_action.rs"
  - path: "packages/kimi-agent/src/tools/memory_repo.rs"
  - path: "packages/kimi-agent/src/tools/memory_store.rs"
  - path: "packages/kimi-agent/src/tools/lsp_tool.rs"
  - path: "packages/kimi-agent/src/tools/session_query.rs"
apis:
  - protocol: rpc
    path: "Skill"
    description:
      zh: >
          按名称加载技能
          
      en: >
          Load a skill by name
          
  - protocol: rpc
    path: "Knowledge"
    description:
      zh: >
          查询知识库
          
      en: >
          Query the knowledge base
          
  - protocol: rpc
    path: "Memory"
    description:
      zh: >
          搜索与管理跨会话记忆（action：search/read/write/list/delete/log/revert）
          
      en: >
          Search and manage cross-session memory (action: search/read/write/list/delete/log/revert)
          
  - protocol: rpc
    path: "lsp"
    description:
      zh: >
          语言服务查询（实验性，默认关闭）
          
      en: >
          Language-server queries (experimental, off by default)
          
---
