---
uid: e1e00008
id: kimi-code.engine.tools.web
parent: kimi-code.engine.tools
name: {zh: "网络与抓取", en: "Web & fetch"}
description:
  zh: >
      注册工具名：WebSearch、FetchURL、SwitchSearchEngine、NotifyUser 与 GitHub* 族。
      
  en: >
      Registered tool names: WebSearch, FetchURL, SwitchSearchEngine, NotifyUser and the GitHub* family.
      
revision: 8c50a0dfdddde7e560ee7637ad3b533551547ccc
updated_at: "2026-11-09T19:52:51.323Z"
fingerprint: 579fcacccafa42390f0a54c9e92c987108c76c984a7ca9b841ac80518407998e
source:
  - path: "packages/kimi-agent/src/tools/web_search.rs"
  - path: "packages/kimi-agent/src/tools/fetch_url.rs"
  - path: "packages/kimi-agent/src/tools/github.rs"
  - path: "packages/kimi-agent/src/tools/github/execute.rs"
  - path: "packages/kimi-agent/src/tools/github/http.rs"
  - path: "packages/kimi-agent/src/tools/github/schema.rs"
  - path: "packages/kimi-agent/src/tools/github/specs.rs"
  - path: "packages/kimi-agent/src/tools/read_media.rs"
apis:
  - protocol: rpc
    path: "WebSearch"
    description:
      zh: >
          搜索网页
          
      en: >
          Search the web
          
  - protocol: rpc
    path: "FetchURL"
    description:
      zh: >
          抓取 URL 为 markdown
          
      en: >
          Fetch a URL as markdown
          
  - protocol: rpc
    path: "SwitchSearchEngine"
    description:
      zh: >
          切换搜索引擎
          
      en: >
          Switch the search engine
          
  - protocol: rpc
    path: "GitHubGetPR"
    description:
      zh: >
          GitHub API 操作
          
      en: >
          GitHub API operations
          
---
