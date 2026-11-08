---
uid: e1e00009
id: kimi-code.engine.server.http
parent: kimi-code.engine.server
name: {zh: "REST API", en: "REST API"}
description:
  zh: >
      REST 面：插件/工作区/会话/配置路由、fs 路由、鉴权与宿主守卫。
      
  en: >
      REST surface: plugin/workspace/session/config routes, fs routes, auth and host guard.
      
revision: 3b18eb137f8d3057962e2f89ee9e34901ced8df4
updated_at: "2026-11-07T08:11:35.101Z"
fingerprint: 2dbf96dff29ab41b944097b3b9d5c730bd807f7b8700b940b73c96289bd9aa06
source:
  - path: "packages/kimi-agent/src/server/mod.rs"
  - path: "packages/kimi-agent/src/server/tests/mod.rs"
  - path: "packages/kimi-agent/src/server/tests/common.rs"
  - path: "packages/kimi-agent/src/server/tests/core_http.rs"
  - path: "packages/kimi-agent/src/server/tests/cron_tasks.rs"
  - path: "packages/kimi-agent/src/server/tests/events_fs_terminal_subagents.rs"
  - path: "packages/kimi-agent/src/server/tests/files_mcp_fs.rs"
  - path: "packages/kimi-agent/src/server/tests/mcp_auth_transcript.rs"
  - path: "packages/kimi-agent/src/server/tests/meta_config_fork.rs"
  - path: "packages/kimi-agent/src/server/tests/patch_undo_compact.rs"
  - path: "packages/kimi-agent/src/server/tests/plugins_search_oauth.rs"
  - path: "packages/kimi-agent/src/server/tests/profile_fs_content.rs"
  - path: "packages/kimi-agent/src/server/tests/prompt_content.rs"
  - path: "packages/kimi-agent/src/server/tests/prompts_providers.rs"
  - path: "packages/kimi-agent/src/server/tests/security.rs"
  - path: "packages/kimi-agent/src/server/tests/session_actions.rs"
  - path: "packages/kimi-agent/src/server/tests/web_contract_export.rs"
  - path: "packages/kimi-agent/src/server/tests/workspaces_models.rs"
  - path: "packages/kimi-agent/src/server/auth.rs"
  - path: "packages/kimi-agent/src/server/fs_routes.rs"
apis:
  - protocol: http
    method: GET
    path: "/api/v1/plugins"
    description:
      zh: >
          列出已安装插件
          
      en: >
          List installed plugins
          
  - protocol: http
    method: POST
    path: "/api/v1/plugins"
    description:
      zh: >
          安装、启用、禁用或移除插件
          
      en: >
          Install / enable / disable / remove a plugin
          
  - protocol: http
    method: GET
    path: "/api/v1/files"
    description:
      zh: >
          静态文件与文件系统路由
          
      en: >
          Serve static files and fs routes
          
---
