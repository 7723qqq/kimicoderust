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
      
revision: e32b1f03dd34aa2dcfbb07086bb81fef08193771
updated_at: "2026-10-31T16:36:39.130Z"
fingerprint: 0acc8362467f66353c9e22d4299c3fabbcf7d70a1dd65d4f4924d9b5b7e8d260
source:
  - path: "packages/kimi-agent/src/server/mod.rs"
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
