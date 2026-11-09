---
uid: e1e0b431
id: kimi-code.engine.tools.basics
parent: kimi-code.engine.tools
name: {zh: "核心文件与 shell 工具", en: "Core file & shell tools"}
description:
  zh: >
      注册核心工具名：Read、Write、Edit、Bash、PowerShell（仅 Windows）、Grep、Glob、run_code、EnterSpecMode/ExitSpecMode 与 goal/cron/task/ask 族。
      
  en: >
      Registered core tool names: Read, Write, Edit, Bash, PowerShell (Windows only), Grep, Glob, run_code, EnterSpecMode/ExitSpecMode and the goal/cron/task/ask families.
      
revision: 8c50a0dfdddde7e560ee7637ad3b533551547ccc
updated_at: "2026-11-09T19:52:51.321Z"
fingerprint: a52381a0f8359ab8d58c3cc5b061c65ad71a79be3ebf3a207c4f7612f92dded6
source:
  - path: "packages/kimi-agent/src/tools/core_tool_defs.rs"
  - path: "packages/kimi-agent/src/tools/sandbox.rs"
  - path: "packages/kimi-agent/src/tools/powershell.rs"
  - path: "packages/kimi-agent/src/tools/goal_tools.rs"
  - path: "packages/kimi-agent/src/tools/cron_tools.rs"
  - path: "packages/kimi-agent/src/tools/run_code.rs"
  - path: "packages/kimi-agent/src/tools/spec_mode.rs"
apis:
  - protocol: rpc
    path: "Bash"
    description:
      zh: >
          运行 shell 命令
          
      en: >
          Run a shell command
          
  - protocol: rpc
    path: "PowerShell"
    description:
      zh: >
          执行 Windows PowerShell 脚本（仅 Windows）
          
      en: >
          Run a Windows PowerShell script (Windows only)
          
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
