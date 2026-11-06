---
uid: e1e0b431
id: kimi-code.engine.tools.basics
parent: kimi-code.engine.tools
name: {zh: "核心文件与 shell 工具", en: "Core file & shell tools"}
description:
  zh: >
      注册核心工具名：Read、Write、Edit、Bash、Grep、Glob、ListDirectory 与 goal/cron/task/ask 族。
      
  en: >
      Registered core tool names: Read, Write, Edit, Bash, Grep, Glob, ListDirectory and the goal/cron/task/ask families.
      
revision: 28f4b101d79bba811d53e53662341c0bb0fe93c0
updated_at: "2026-11-06T04:54:04.173Z"
fingerprint: aa94dd830c102f17f69aaaf6e997cb60f68a55738f09d4a20dc143aa9a6305a3
source:
  - path: "packages/kimi-agent/src/tools/core_tool_defs.rs"
  - path: "packages/kimi-agent/src/tools/sandbox.rs"
  - path: "packages/kimi-agent/src/tools/goal_tools.rs"
  - path: "packages/kimi-agent/src/tools/cron_tools.rs"
apis:
  - protocol: rpc
    path: "Bash"
    description:
      zh: >
          运行 shell 命令
          
      en: >
          Run a shell command
          
  - protocol: rpc
    path: "Read"
    description:
      zh: >
          读取文件
          
      en: >
          Read a file
          
  - protocol: rpc
    path: "Edit"
    description:
      zh: >
          编辑文件
          
      en: >
          Edit a file
          
  - protocol: rpc
    path: "Write"
    description:
      zh: >
          写入文件
          
      en: >
          Write a file
          
  - protocol: rpc
    path: "Grep"
    description:
      zh: >
          搜索文件内容
          
      en: >
          Search file contents
          
---
